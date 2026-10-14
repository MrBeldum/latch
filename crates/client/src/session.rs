//! Finding PCs on the tailnet, and the path from "asleep in another room" to
//! a live stream: wake, wait, pair if needed, launch, connect.

use crate::clipboard;
use crate::config::{ClientConfig, Codec, KnownPc, StreamSettings};
use crate::path::{self, Path};
use anyhow::{anyhow, bail, Result};
use latch_core::api::{Ack, NatReport, PinRequest, PowerAction, PowerRequest, Status};
use latch_core::{http, tailscale, wake, CONTROL_PORT, SUNSHINE_PORT};
use latch_stream::session::Server;
use latch_stream::{Client, Event, FrameSlot, Identity, Input, Session, Settings};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Each [`connect`] call gets a new id so a cancelled worker cannot finish
/// into a later attempt's `progress` / `live`.
static NEXT_CONNECT: AtomicU64 = AtomicU64::new(1);

/// How often this Mac's own NAT is re-examined. `tailscale netcheck` takes
/// seconds and networks change when the Mac moves, not more often.
const NETCHECK_EVERY: Duration = Duration::from_secs(15 * 60);

/// How often the debug command is polled for the granted relay set. It is a
/// cheap local call; the ACL changes rarely, so 30 s is plenty.
const PEER_RELAY_EVERY: Duration = Duration::from_secs(30);

/// A machine on the tailnet Latch can open, as far as this node can tell.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pc {
    pub node_id: String,
    pub name: String,
    /// Tailscale OS string ("windows", "macOS", "linux").
    pub os: String,
    pub ip: Option<Ipv4Addr>,
    /// Tailscale's view; lags a wake-up by up to half a minute.
    pub online: bool,
    /// The Latch control service answered.
    pub host: Option<Status>,
    /// Sunshine's port answered.
    pub sunshine: bool,
    pub known: Option<KnownPc>,
    /// Listed from what was saved, because Tailscale on this Mac could not
    /// say; nothing has been probed.
    pub remembered: bool,
    /// Days until the PC's Tailscale key expires; `None` when it never does.
    pub key_expiry_days: Option<i64>,
    /// Direct or relayed, and how far away.
    pub path: Path,
}

impl Pc {
    pub fn can_wake(&self) -> bool {
        self.known.as_ref().and_then(|k| k.mac.as_ref()).is_some()
    }
    pub fn can_stream(&self) -> bool {
        self.ip.is_some() && (self.sunshine || self.host.is_some() || self.can_wake())
    }
    pub fn power_allowed(&self) -> bool {
        self.host.as_ref().is_some_and(|h| h.power_allowed)
    }
}

/// A tailnet node tagged `tag:relay`: a candidate peer relay, never a PC.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Relay {
    pub name: String,
    pub ip: Option<Ipv4Addr>,
    pub online: bool,
}

/// The relay server set from `tailscale debug peer-relay-servers`. `Unknown`
/// when the command could not run (CLI missing, timeout, garbage) — a failed
/// debug probe proves nothing about the ACL. `Known` is a real answer: the
/// IPs this node may use right now. Empty is not proof of ACL denial — the
/// tagged node may simply not be configured as a relay. Non-empty is the
/// permitted set, not a promise that a `tag:relay` node matches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PeerRelayServers {
    #[default]
    Unknown,
    Known(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Discovery {
    /// Why nothing can be probed, when Tailscale is down. The list then
    /// holds what was remembered.
    pub error: Option<String>,
    pub login: String,
    pub pcs: Vec<Pc>,
    pub refreshed: Option<Instant>,
    /// Days until this Mac's own Tailscale key expires.
    pub self_key_days: Option<i64>,
    /// This Mac's side of the NAT story, once `tailscale netcheck` has run.
    pub self_nat: Option<NatReport>,
    /// This Mac's own Tailscale address, the one a PC can reach.
    pub self_ip: Option<Ipv4Addr>,
    /// Relay nodes on the tailnet, from `status --json`. Never streamable.
    pub relays: Vec<Relay>,
    /// Relay servers this device is granted to use, from the debug command.
    pub peer_relay_servers: PeerRelayServers,
}

/// Rescan the tailnet every few seconds and remember what each PC needs to
/// be woken.
pub fn spawn_discovery(shared: Arc<Mutex<Discovery>>, ctx: egui::Context) {
    let nat: Arc<Mutex<Option<NatReport>>> = Arc::default();
    spawn_netcheck(nat.clone());
    let servers: Arc<Mutex<PeerRelayServers>> = Arc::default();
    spawn_peer_relay_probe(servers.clone());
    std::thread::spawn(move || {
        // Round trips per PC, newest last; the smallest of the last few is
        // the path's real round trip (a first connect also pays for the
        // WireGuard handshake).
        let mut rtts: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        loop {
            let cfg = ClientConfig::load();
            let mut scan = scan(&cfg);
            for pc in &mut scan.pcs {
                if let Some(ms) = pc.path.rtt_ms {
                    let h = rtts.entry(pc.node_id.clone()).or_default();
                    h.push(ms);
                    if h.len() > 5 {
                        h.remove(0);
                    }
                    pc.path.rtt_ms = h.iter().copied().min();
                }
            }
            scan.self_nat = nat.lock().clone();
            scan.peer_relay_servers = servers.lock().clone();
            learn(&scan);
            let changed = listing_changed(&shared.lock(), &scan);
            *shared.lock() = scan;
            if changed {
                ctx.request_repaint();
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

/// Whether a new scan shows the window anything new. Every scan has a new
/// time; that alone is no reason to draw the window again.
fn listing_changed(old: &Discovery, new: &Discovery) -> bool {
    old.refreshed.is_none() != new.refreshed.is_none()
        || *old
            != (Discovery {
                refreshed: old.refreshed,
                ..new.clone()
            })
}

/// `tailscale netcheck` now and then, for [`Discovery::self_nat`]. The next
/// scan carries it to the window.
fn spawn_netcheck(slot: Arc<Mutex<Option<NatReport>>>) {
    std::thread::spawn(move || loop {
        match tailscale::netcheck() {
            Ok(n) => *slot.lock() = Some(n.report()),
            Err(e) => tracing::info!("netcheck: {e}"),
        }
        std::thread::sleep(NETCHECK_EVERY);
    });
}

/// `tailscale debug peer-relay-servers`, at most once every 30 s, on its own
/// thread so the scan loop and UI never wait on it. The command itself is
/// bounded (see `tailscale::PEER_RELAY_SERVERS_TIMEOUT`). Any failure leaves
/// the result `Unknown`, never a denial. The next scan carries it to the
/// window.
fn spawn_peer_relay_probe(slot: Arc<Mutex<PeerRelayServers>>) {
    std::thread::spawn(move || loop {
        *slot.lock() = match tailscale::peer_relay_servers() {
            Ok(servers) => PeerRelayServers::Known(servers),
            Err(_) => PeerRelayServers::Unknown,
        };
        std::thread::sleep(PEER_RELAY_EVERY);
    });
}

/// Save what a scan taught about each PC: address, wake details, when it
/// was last seen.
fn learn(scan: &Discovery) {
    if let Err(e) = ClientConfig::update(|learned| {
        for pc in &scan.pcs {
            if pc.remembered {
                continue;
            }
            let entry = learned.pcs.entry(pc.node_id.clone()).or_default();
            entry.name = pc.name.clone();
            if !pc.os.is_empty() {
                entry.os = pc.os.clone();
            }
            if let Some(ip) = pc.ip {
                entry.tailscale_ip = Some(ip.to_string());
            }
            if pc.online {
                // Coarse on purpose: the file is rewritten only when this moves.
                let now = latch_core::dates::now_unix();
                if entry
                    .last_seen_unix
                    .is_none_or(|t| now.saturating_sub(t) > 600)
                {
                    entry.last_seen_unix = Some(now);
                }
            }
            if let Some(h) = &pc.host {
                if h.mac.is_some() {
                    entry.mac = h.mac.clone();
                    entry.lan_ip = h.lan_ip.clone();
                }
            }
            if let Some(p) = scan_public(pc) {
                entry.public_ip = Some(p.to_string());
            }
        }
    }) {
        tracing::warn!("could not save what was learned: {e:#}");
    }
}

fn scan_public(pc: &Pc) -> Option<Ipv4Addr> {
    pc.known.as_ref()?.public_ip.as_deref()?.parse().ok()
}

pub fn scan(cfg: &ClientConfig) -> Discovery {
    let st = match tailscale::status() {
        Ok(s) => s,
        Err(e) => {
            return Discovery {
                error: Some(e.to_string()),
                pcs: remembered(cfg),
                refreshed: Some(Instant::now()),
                ..Default::default()
            }
        }
    };
    let mut peers = st.machine_peers();
    let local;
    if std::env::var_os("LATCH_DEV_LOCAL").is_some() {
        local = tailscale::Node {
            id: "local".into(),
            host_name: format!("{} (this machine)", latch_core::config::machine_name()),
            os: std::env::consts::OS.into(),
            tailscale_ips: vec!["127.0.0.1".into()],
            online: true,
            ..Default::default()
        };
        peers.push(&local);
    }
    let mut pcs: Vec<Pc> = peers.into_iter().map(|n| probe_peer(cfg, n)).collect();
    // A tag:relay node that also runs Latch (a VPS desktop on the same
    // box as the packet relay) belongs in the machine list too.
    for n in st.relay_peers() {
        let pc = probe_peer(cfg, n);
        if pc.host.is_some() || pc.sunshine {
            pcs.push(pc);
        }
    }
    pcs.sort_by(|a, b| a.name.cmp(&b.name));
    let relays = st
        .relay_peers()
        .into_iter()
        .map(|n| Relay {
            name: n.host_name.clone(),
            ip: n.ipv4(),
            online: n.online,
        })
        .collect();
    Discovery {
        error: None,
        login: st.self_login().unwrap_or("").to_string(),
        pcs,
        refreshed: Some(Instant::now()),
        self_key_days: st.self_node.key_expiry_days(),
        self_nat: None,
        self_ip: st.self_node.ipv4(),
        relays,
        peer_relay_servers: PeerRelayServers::default(),
    }
}

/// The PCs as last saved, for when Tailscale cannot list them. Sorted by
/// name like the live list.
pub fn remembered(cfg: &ClientConfig) -> Vec<Pc> {
    let mut pcs: Vec<Pc> = cfg
        .pcs
        .iter()
        .filter(|(_, k)| !k.name.is_empty())
        .map(|(id, k)| Pc {
            node_id: id.clone(),
            name: k.name.clone(),
            os: k.os.clone(),
            ip: k.tailscale_ip.as_deref().and_then(|s| s.parse().ok()),
            known: Some(k.clone()),
            remembered: true,
            ..Default::default()
        })
        .collect();
    pcs.sort_by(|a, b| a.name.cmp(&b.name));
    pcs
}

fn probe_peer(cfg: &ClientConfig, n: &tailscale::Node) -> Pc {
    let ip = n.ipv4();
    let (host, sunshine, rtt_ms) = match (n.online, ip) {
        (true, Some(ip)) => {
            let host = host_status(ip, Duration::from_millis(900));
            let (sunshine, rtt) = timed_port_open(ip, SUNSHINE_PORT, Duration::from_millis(900));
            (host, sunshine, rtt)
        }
        _ => (None, false, None),
    };
    let mut known = cfg.pcs.get(&n.id).cloned();
    if let Some(p) = n.public_ipv4() {
        known.get_or_insert_with(Default::default).public_ip = Some(p.to_string());
    }
    Pc {
        node_id: n.id.clone(),
        name: n.host_name.clone(),
        os: n.os.clone(),
        ip,
        online: n.online,
        host,
        sunshine,
        known,
        remembered: false,
        key_expiry_days: n.key_expiry_days(),
        path: Path {
            direct: n.direct(),
            relay: n.relay.clone(),
            peer_relay: n.peer_relay.clone(),
            rtt_ms,
        },
    }
}

fn host_status(ip: Ipv4Addr, timeout: Duration) -> Option<Status> {
    http::get_json::<Status>((ip, CONTROL_PORT), "/v1/status", timeout)
        .ok()
        .filter(|s| latch_core::legacy::is_node_app(&s.app))
}

fn port_open(ip: Ipv4Addr, port: u16, timeout: Duration) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from((ip, port)), timeout).is_ok()
}

/// Whether the port answers, and how long the connect took: one round
/// trip, which is the path's latency.
fn timed_port_open(ip: Ipv4Addr, port: u16, timeout: Duration) -> (bool, Option<u32>) {
    let t = Instant::now();
    let open = port_open(ip, port, timeout);
    (open, open.then(|| t.elapsed().as_millis() as u32))
}

/// A fresh look at the path to `ip` right before connecting: direct or
/// relayed from Tailscale's own view of the peer, latency from the best of
/// a few connects.
fn measure_path(node_id: &str, ip: Ipv4Addr, prior: &Path) -> Path {
    let mut p = prior.clone();
    if let Ok(st) = tailscale::status() {
        if let Some(n) = st.peer.values().find(|n| n.id == node_id) {
            p.peer_relay = n.peer_relay.clone();
            if let Some(d) = n.direct() {
                p.direct = Some(d);
                p.relay = n.relay.clone();
            }
        }
    }
    let mut best: Option<u32> = prior.rtt_ms;
    for _ in 0..3 {
        if let (true, Some(ms)) = timed_port_open(ip, SUNSHINE_PORT, Duration::from_millis(1500)) {
            best = Some(best.map_or(ms, |b| b.min(ms)));
        }
    }
    p.rtt_ms = best;
    p
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Idle,
    Waking,
    Waiting,
    Pairing {
        pin: String,
    },
    Launching,
    /// The stream is being negotiated; the text is the current stage.
    Connecting,
    Streaming,
    Ended {
        error: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub pc: String,
    pub step: Step,
    pub detail: String,
    pub since: Instant,
    pub cancel: bool,
    /// Blocks new connections while the updater swaps and relaunches this app.
    pub updating: bool,
    /// Sunshine's app names, once listed.
    pub apps: Vec<String>,
    /// Distinguishes this attempt from an older worker that is still running.
    pub generation: u64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            pc: String::new(),
            step: Step::Idle,
            detail: String::new(),
            since: Instant::now(),
            cancel: false,
            updating: false,
            apps: Vec::new(),
            generation: 0,
        }
    }
}

impl Progress {
    pub fn active(&self) -> bool {
        self.updating || !matches!(self.step, Step::Idle | Step::Ended { .. })
    }
    fn set(&mut self, step: Step, detail: impl Into<String>) {
        if self.step != step {
            self.since = Instant::now();
        }
        self.step = step;
        self.detail = detail.into();
    }
}

/// A running stream and everything the window needs to show and drive it.
pub struct Live {
    pub pc: String,
    pub node_id: String,
    pub ip: Ipv4Addr,
    pub session: Session,
    /// The session's input side, for the view and for workers.
    pub input: Input,
    pub frames: Arc<FrameSlot>,
    pub events: Receiver<Event>,
    pub started: Instant,
    pub codec: &'static str,
    pub requested: (u32, u32, u32),
    /// The PC's desktop, when it is a fixed size other than the stream's
    /// (the stream is that desktop scaled down). Relative mouse movement
    /// lands in its pixels, not the stream's.
    pub desktop: Option<(u32, u32)>,
    /// The settings the stream was started with, after Auto had its say.
    pub settings: StreamSettings,
    /// The path as measured right before connecting.
    pub path: Path,
    /// Clipboard both ways, through Latch Host on the PC.
    pub clipboard: clipboard::Sync,
    /// How the PC spells copy, cut and paste.
    pub shortcuts: clipboard::Shortcuts,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.clipboard.stop();
    }
}

/// Everything the worker needs about the PC, copied so the UI can move on.
#[derive(Debug, Clone)]
pub struct Target {
    pub node_id: String,
    pub name: String,
    pub ip: Ipv4Addr,
    pub online: bool,
    pub mac: Option<wake::MacAddr>,
    pub lan_ip: Option<Ipv4Addr>,
    pub public_ip: Option<Ipv4Addr>,
    pub server_cert: Option<Vec<u8>>,
    /// What discovery last knew about the path; re-measured at connect.
    pub path: Path,
    /// Tailscale's OS string for the PC.
    pub os: String,
    /// The PC's fixed capture size and frame-rate ceiling, from its last
    /// status; see [`fit_to_host`].
    pub capture_size: Option<[u32; 2]>,
    pub max_fps: u32,
}

impl Target {
    pub fn from_pc(pc: &Pc) -> Option<Self> {
        let k = pc.known.as_ref();
        let parse = |s: Option<&String>| s.and_then(|s| s.parse().ok());
        Some(Self {
            node_id: pc.node_id.clone(),
            name: pc.name.clone(),
            ip: pc.ip?,
            online: pc.online && (pc.sunshine || pc.host.is_some()),
            mac: k
                .and_then(|k| k.mac.as_deref())
                .and_then(wake::MacAddr::parse),
            lan_ip: parse(k.and_then(|k| k.lan_ip.as_ref())),
            public_ip: parse(k.and_then(|k| k.public_ip.as_ref())),
            server_cert: k
                .and_then(|k| k.server_cert.as_deref())
                .and_then(latch_stream::nvhttp::unhex),
            path: pc.path.clone(),
            os: pc.os.clone(),
            capture_size: pc.host.as_ref().and_then(|h| h.streamer.capture_size),
            max_fps: pc.host.as_ref().map_or(0, |h| h.streamer.max_fps),
        })
    }
}

/// What to ask a PC for, from what was wanted: its own capture size when
/// that is fixed and the wanted size is larger in either direction
/// (Sunshine would only scale the same desktop up, blurring it and costing
/// the encoder more; a smaller size is scaled down and is cheaper to
/// encode), and no more frames a second than it keeps up with. The host's
/// ceiling is for its capture size, or 1920 × 1080 when that is not fixed;
/// a smaller picture gets a ceiling higher in proportion to the pixels it
/// saves, since software encoding time follows the pixel count. A larger
/// one keeps the host's ceiling. Implausible sizes are ignored.
pub fn fit_to_host(
    wanted: (u32, u32),
    fps: u32,
    capture_size: Option<[u32; 2]>,
    max_fps: u32,
) -> (u32, u32, u32) {
    let fixed = fixed_desktop(capture_size);
    let (w, h) = match fixed {
        Some(c) if wanted.0 > c.0 || wanted.1 > c.1 => c,
        _ => wanted,
    };
    let fps = if max_fps > 0 {
        let (rw, rh) = fixed.unwrap_or((1920, 1080));
        let budget = u64::from(max_fps) * u64::from(rw) * u64::from(rh);
        let ceiling = (budget / (u64::from(w) * u64::from(h)).max(1)).max(max_fps.into());
        fps.min(u32::try_from(ceiling).unwrap_or(u32::MAX))
    } else {
        fps
    };
    (w, h, fps)
}

/// A host's fixed capture size, when it reported a plausible one.
fn fixed_desktop(capture_size: Option<[u32; 2]>) -> Option<(u32, u32)> {
    match capture_size {
        Some([w, h]) if (320..=8192).contains(&w) && (240..=8192).contains(&h) => Some((w, h)),
        _ => None,
    }
}

/// The size each PC's Desktop was last launched at from here, by node.
static LAUNCHED: Mutex<BTreeMap<String, (u32, u32)>> = Mutex::new(BTreeMap::new());

/// Whether the running app has to be ended before this launch. Another app
/// always does. The Desktop does when asked to (a broken capture) and when
/// it may be running at another size: resuming keeps the PC's display as
/// the launch left it, since a host's prep command (a Linux desktop
/// resizing itself to the viewer) runs only at a launch, so a stream at a
/// new size would be the old desktop scaled, with the mouse moving over
/// the old one. A Desktop this Mac did not launch, or launched before it
/// was last opened, is of unknown size.
fn must_relaunch(
    current_game: u32,
    app_id: u32,
    desktop: bool,
    restart_capture: bool,
    launched_at: Option<(u32, u32)>,
    size: (u32, u32),
) -> bool {
    current_game != 0
        && (current_game != app_id || (desktop && (restart_capture || launched_at != Some(size))))
}

pub struct Connect {
    pub target: Target,
    pub settings: StreamSettings,
    /// Reset Sunshine's Desktop capture instead of resuming a broken one.
    pub restart_capture: bool,
    pub native: (u32, u32),
    pub progress: Arc<Mutex<Progress>>,
    pub live: Arc<Mutex<Option<Live>>>,
    pub ctx: egui::Context,
}

/// Wake, wait, pair, launch, connect: on its own thread, reporting into
/// `progress` and leaving the stream in `live`.
pub fn connect(c: Connect) {
    let generation = NEXT_CONNECT.fetch_add(1, Ordering::Relaxed);
    {
        let mut p = c.progress.lock();
        if p.active() {
            return;
        }
        *p = Progress {
            pc: c.target.name.clone(),
            generation,
            ..Default::default()
        };
        p.set(Step::Launching, "Starting…");
    }
    std::thread::spawn(move || {
        let result = run(&c, generation);
        let mut p = c.progress.lock();
        if p.generation != generation {
            return;
        }
        match result {
            Ok(()) => {}
            Err(e) if p.cancel => {
                tracing::info!("cancelled: {e:#}");
                p.set(Step::Ended { error: None }, "");
            }
            Err(e) => p.set(
                Step::Ended {
                    error: Some(format!("{e}")),
                },
                "",
            ),
        }
        c.ctx.request_repaint();
    });
}

fn stale(progress: &Mutex<Progress>, generation: u64) -> bool {
    let p = progress.lock();
    p.cancel || p.generation != generation
}

fn run(c: &Connect, generation: u64) -> Result<()> {
    let t = &c.target;
    let report = |step: Step, detail: String| {
        c.progress.lock().set(step, detail);
        c.ctx.request_repaint();
    };

    // 1. Wake it if nothing answers.
    if !t.online && !port_open(t.ip, SUNSHINE_PORT, Duration::from_millis(1200)) {
        let mac = t.mac.ok_or_else(|| {
            anyhow!("{} isn't answering, and Latch doesn't know how to wake it yet. Turn it on once while Latch runs there, so this machine can learn how.", t.name)
        })?;
        let start = Instant::now();
        let mut last_wake = Instant::now() - Duration::from_secs(60);
        loop {
            if stale(&c.progress, generation) {
                bail!("cancelled");
            }
            if last_wake.elapsed() >= Duration::from_secs(5) {
                let n = wake::send(mac, t.lan_ip, t.public_ip);
                tracing::info!("sent {n} wake packets for {mac}");
                last_wake = Instant::now();
            }
            report(
                Step::Waking,
                format!("Waking {}… {}s", t.name, start.elapsed().as_secs()),
            );
            if port_open(t.ip, SUNSHINE_PORT, Duration::from_millis(1500))
                || port_open(t.ip, CONTROL_PORT, Duration::from_millis(1500))
            {
                break;
            }
            if start.elapsed() > Duration::from_secs(120) {
                bail!(
                    "{} didn't wake up. A wake packet reaches it only from its own network, or through a router that forwards UDP port 9 to it. Its Sharing tab in Latch shows whether Wake-on-LAN is ready.",
                    t.name
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    // 2. Sunshine's port.
    let start = Instant::now();
    while !port_open(t.ip, SUNSHINE_PORT, Duration::from_millis(1500)) {
        if stale(&c.progress, generation) {
            bail!("cancelled");
        }
        report(
            Step::Waiting,
            format!("Waiting for {}… {}s", t.name, start.elapsed().as_secs()),
        );
        if start.elapsed() > Duration::from_secs(60) {
            bail!(
                "{} is on, but nothing is streaming from it. Open Latch there and set up sharing on its Sharing tab.",
                t.name
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    // 3. Pair if this Mac is not known to the PC yet.
    report(Step::Launching, "Checking pairing…".into());
    let identity = Identity::load_or_create(&latch_core::config::data_dir()?.join("identity"))?;
    let mut client = Client::new(&identity, IpAddr::V4(t.ip), t.server_cert.clone())?;
    let mut info = match client.server_info() {
        Ok(i) => i,
        Err(e) if latch_stream::nvhttp::is_pin_mismatch(&e) => {
            // The engine was reinstalled; the saved cert is the old one.
            // Forget it and pair again so the user does not have to edit
            // client.toml.
            tracing::warn!("{}: {e:#}; pairing again", t.name);
            if let Err(save_error) = ClientConfig::update(|cfg| {
                cfg.forget_pin_on_mismatch(&t.node_id, &e);
            }) {
                tracing::warn!("could not forget the PC's old certificate: {save_error:#}");
            }
            client = Client::new(&identity, IpAddr::V4(t.ip), None)?;
            retry(8, &c.progress, generation, || client.server_info())?
        }
        Err(_) => retry(8, &c.progress, generation, || client.server_info())?,
    };
    if !info.paired || client.server_cert().is_none() {
        let pin = format!("{:04}", rand::random::<u16>() % 10_000);
        report(Step::Pairing { pin: pin.clone() }, String::new());
        let der = client.pair_cancellable(&pin, &latch_core::config::machine_name(), || {
            submit_pin(t.ip, &pin, &c.progress, &c.ctx);
            stale(&c.progress, generation)
        })?;
        remember_cert(&t.node_id, &t.name, &der);
        info = client.server_info()?;
        if !info.paired {
            bail!("{} did not accept the pairing", t.name);
        }
    }
    if stale(&c.progress, generation) {
        bail!("cancelled");
    }

    // 4. Pick the app and launch or resume it, at a quality the path can
    //    carry.
    report(Step::Launching, "Measuring the path…".into());
    let path = measure_path(&t.node_id, t.ip, &t.path);
    let mut settings = path::effective(&c.settings);
    let wanted = settings.resolution.pixels(c.native);
    let (w, h, fps) = fit_to_host(wanted, settings.fps, t.capture_size, t.max_fps);
    settings.fps = fps;
    tracing::info!(
        "path to {}: {} · {}{}",
        t.name,
        path.label(),
        settings.describe(),
        if (w, h) != wanted {
            format!(" · {w} × {h}, the size {} draws", t.name)
        } else {
            String::new()
        }
    );
    report(Step::Launching, "Starting the stream…".into());
    let apps = client.app_list()?;
    c.progress.lock().apps = apps.iter().map(|a| a.title.clone()).collect();
    let app = apps
        .iter()
        .find(|a| a.title.eq_ignore_ascii_case(&settings.app))
        .or_else(|| apps.iter().find(|a| a.title == "Desktop"))
        .or_else(|| apps.first())
        .ok_or_else(|| anyhow!("{} offers nothing to stream", t.name))?;
    let desktop = app.title.eq_ignore_ascii_case("Desktop");
    let launched_at = LAUNCHED.lock().get(&t.node_id).copied();
    if must_relaunch(
        info.current_game,
        app.id,
        desktop,
        c.restart_capture,
        launched_at,
        (w, h),
    ) {
        client.quit()?;
        let stopped = Instant::now();
        loop {
            if stale(&c.progress, generation) {
                bail!("cancelled");
            }
            info = client.server_info()?;
            if info.current_game == 0 {
                break;
            }
            if stopped.elapsed() >= Duration::from_secs(10) {
                bail!(
                    "{} didn't stop its previous stream. Try again; if it keeps happening, restart that machine.",
                    t.name
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    let ri_key: [u8; 16] = rand::random();
    let ri_id: u32 = rand::random();
    let mut ri_iv = [0u8; 16];
    ri_iv[..4].copy_from_slice(&ri_id.to_be_bytes());
    let rtsp = client.launch(
        app.id,
        w,
        h,
        fps,
        settings.bitrate_kbps,
        &ri_key,
        ri_id,
        info.current_game != 0,
    )?;
    if desktop {
        LAUNCHED.lock().insert(t.node_id.clone(), (w, h));
    }

    // 5. Connect.
    if stale(&c.progress, generation) {
        let _ = client.quit();
        bail!("cancelled");
    }
    let hevc = wants_hevc(settings.codec, info.codec_mode_support);
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = c.ctx.clone();
    let session = Session::start(
        Server {
            address: t.ip.to_string(),
            app_version: info.app_version.clone(),
            gfe_version: info.gfe_version.clone(),
            rtsp_url: rtsp,
            codec_mode_support: info.codec_mode_support,
        },
        Settings {
            width: w,
            height: h,
            fps,
            bitrate_kbps: settings.bitrate_kbps,
            hevc,
        },
        ri_key,
        ri_iv,
        frames.clone(),
        tx,
        move || ctx.request_repaint(),
    );
    if stale(&c.progress, generation) {
        drop(session);
        let _ = client.quit();
        bail!("cancelled");
    }
    let input = session.input();
    *c.live.lock() = Some(Live {
        pc: t.name.clone(),
        node_id: t.node_id.clone(),
        ip: t.ip,
        session,
        input,
        frames,
        events: rx,
        started: Instant::now(),
        codec: if hevc { "HEVC" } else { "H.264" },
        requested: (w, h, fps),
        desktop: fixed_desktop(t.capture_size).filter(|&d| d != (w, h)),
        settings,
        path,
        clipboard: clipboard::Sync::spawn(t.ip, c.ctx.clone()),
        shortcuts: clipboard::Shortcuts::for_os(&t.os),
    });
    report(Step::Connecting, "Connecting…".into());
    Ok(())
}

/// HEVC when the settings allow it, the PC can encode it (Sunshine's
/// `ServerCodecModeSupport` HEVC bits) and this machine can decode it.
/// Windows and Linux decode H.264 only, whatever the PC offers.
fn wants_hevc(codec: Codec, server_codecs: i32) -> bool {
    const SCM_HEVC_MASK: i32 = 0x0F00;
    codec != Codec::H264
        && server_codecs & SCM_HEVC_MASK != 0
        && latch_stream::video::supported_formats() & latch_stream::ffi::VIDEO_FORMAT_MASK_H265 != 0
}

/// `f` until it succeeds, `times` tries at most, giving up early once the
/// attempt is cancelled.
fn retry<T>(
    times: u32,
    progress: &Mutex<Progress>,
    generation: u64,
    mut f: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut last = anyhow!("not tried");
    for attempt in 1..=times {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
        if attempt == times || stale(progress, generation) {
            break;
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    Err(last)
}

/// Hand the PIN to Latch Host on the PC, which types it into Sunshine. If
/// there is no Latch Host, the PIN stays on screen for someone at the PC.
fn submit_pin(ip: Ipv4Addr, pin: &str, progress: &Mutex<Progress>, ctx: &egui::Context) {
    let req = PinRequest {
        pin: pin.into(),
        name: latch_core::config::machine_name(),
    };
    // Pairing draws no spinner, so a new reason has to ask for its frame.
    let show = |detail: String| {
        let mut p = progress.lock();
        if p.detail != detail {
            p.detail = detail;
            drop(p);
            ctx.request_repaint();
        }
    };
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(75) {
        if progress.lock().cancel {
            return;
        }
        // Latch Host answers once the engine has the PIN: up to a little
        // over half a second with an engine that holds the post open.
        match http::post_json::<_, Ack>((ip, CONTROL_PORT), "/v1/pin", &req, Duration::from_secs(3))
        {
            Ok(a) if a.ok => return,
            Ok(a) => {
                let reason = a.error.unwrap_or_else(|| "PIN not accepted yet".into());
                tracing::info!("PIN not accepted yet: {reason}");
                show(reason);
            }
            Err(e) => {
                tracing::info!("Latch Host did not take the PIN: {e}");
                show("Latch on that machine isn't answering, so it can't enter the PIN. Open Latch there and set up sharing, then try again.".into());
            }
        }
        std::thread::sleep(Duration::from_millis(800));
    }
}

fn remember_cert(node_id: &str, name: &str, der: &[u8]) {
    if let Err(e) = ClientConfig::update(|cfg| {
        let e = cfg.pcs.entry(node_id.to_string()).or_default();
        e.name = name.to_string();
        e.server_cert = Some(latch_stream::nvhttp::hex(der));
    }) {
        tracing::warn!("could not save the PC's certificate: {e:#}");
    }
}

/// Ask the host to sleep, restart, or shut down.
pub fn power(ip: Ipv4Addr, action: PowerAction) -> Result<()> {
    let a: Ack = http::post_json(
        (ip, CONTROL_PORT),
        "/v1/power",
        &PowerRequest { action },
        Duration::from_secs(6),
    )?;
    if a.ok {
        Ok(())
    } else {
        bail!("{}", a.error.unwrap_or_else(|| "refused".into()))
    }
}

/// Send the wake packet to a PC that is awake and ask Latch Host on it
/// whether the packet arrived: the same path a real wake would take.
pub fn wake_test(pc: &Pc) -> Result<bool> {
    let ip = pc.ip.ok_or_else(|| anyhow!("no address for {}", pc.name))?;
    wake_only(pc)?;
    std::thread::sleep(Duration::from_millis(1500));
    let st = host_status(ip, Duration::from_secs(3))
        .ok_or_else(|| anyhow!("Latch on {} isn't answering", pc.name))?;
    Ok(st.wake_packet_age_secs.is_some_and(|s| s <= 5))
}

/// Send the wake packet once, without connecting.
pub fn wake_only(pc: &Pc) -> Result<usize> {
    let t = Target::from_pc(pc).ok_or_else(|| anyhow!("no address for {}", pc.name))?;
    let mac = t.mac.ok_or_else(|| {
        anyhow!(
            "Latch doesn't know how to wake {} yet; turn it on once while Latch runs there",
            pc.name
        )
    })?;
    Ok(wake::send(mac, t.lan_ip, t.public_ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desktop_running_at_another_size_is_launched_again() {
        const DESKTOP: u32 = 7;
        let p720 = (1280, 720);
        // Nothing running: a plain launch.
        assert!(!must_relaunch(0, DESKTOP, true, true, None, p720));
        // Launched from here at this size: resumed.
        assert!(!must_relaunch(
            DESKTOP,
            DESKTOP,
            true,
            false,
            Some(p720),
            p720
        ));
        // At another size, or at a size this Mac does not know: launched
        // again, so the PC's display follows the new size.
        assert!(must_relaunch(
            DESKTOP,
            DESKTOP,
            true,
            false,
            Some((3024, 1964)),
            p720
        ));
        assert!(must_relaunch(DESKTOP, DESKTOP, true, false, None, p720));
        // A broken capture, or another app, always.
        assert!(must_relaunch(
            DESKTOP,
            DESKTOP,
            true,
            true,
            Some(p720),
            p720
        ));
        assert!(must_relaunch(9, DESKTOP, true, false, Some(p720), p720));
        // A game is never ended for a size change.
        assert!(!must_relaunch(9, 9, false, false, None, p720));
        assert!(!must_relaunch(9, 9, false, true, None, p720));
    }

    #[test]
    fn a_fixed_desktop_is_asked_for_at_its_own_size_and_frame_rate() {
        // A VPS draws 1920 × 1080 and encodes in software on two cores.
        let vps = (Some([1920, 1080]), 30);
        assert_eq!(
            fit_to_host((2284, 1208), 60, vps.0, vps.1),
            (1920, 1080, 30)
        );
        assert_eq!(
            fit_to_host((1920, 1080), 30, vps.0, vps.1),
            (1920, 1080, 30)
        );
        assert_eq!(
            fit_to_host((3840, 2160), 24, vps.0, vps.1),
            (1920, 1080, 24)
        );
        assert_eq!(
            fit_to_host((2560, 1080), 30, vps.0, vps.1),
            (1920, 1080, 30),
            "wider than the desktop is scaling up too"
        );
        // A smaller size is scaled down, and fewer pixels keep up with more
        // frames: 720p is 2.25 times fewer than 1080p.
        assert_eq!(fit_to_host((1280, 720), 60, vps.0, vps.1), (1280, 720, 60));
        assert_eq!(fit_to_host((1600, 900), 60, vps.0, vps.1), (1600, 900, 43));
        assert_eq!(fit_to_host((960, 540), 144, vps.0, vps.1), (960, 540, 120));
        // A desktop that takes the viewer's size: the ceiling is for 1080p.
        let follows = (None, 30);
        assert_eq!(
            fit_to_host((1280, 720), 60, follows.0, follows.1),
            (1280, 720, 60)
        );
        assert_eq!(
            fit_to_host((1920, 1080), 60, follows.0, follows.1),
            (1920, 1080, 30)
        );
        assert_eq!(
            fit_to_host((3024, 1964), 60, follows.0, follows.1),
            (3024, 1964, 30),
            "a larger picture keeps the host's ceiling"
        );
        // A Windows PC matches the viewer's size itself and has no ceiling.
        assert_eq!(fit_to_host((2284, 1208), 60, None, 0), (2284, 1208, 60));
        // Nonsense from a host is ignored.
        assert_eq!(
            fit_to_host((1920, 1080), 60, Some([0, 0]), 0),
            (1920, 1080, 60)
        );
        assert_eq!(
            fit_to_host((1920, 1080), 60, Some([100_000, 1080]), 0),
            (1920, 1080, 60)
        );
    }

    #[test]
    fn a_target_carries_the_hosts_limits_and_os() {
        let pc = Pc {
            node_id: "n".into(),
            name: "artemis".into(),
            os: "linux".into(),
            ip: Some("100.64.0.9".parse().unwrap()),
            online: true,
            host: Some(Status {
                streamer: latch_core::api::Streamer {
                    capture_size: Some([1920, 1080]),
                    max_fps: 30,
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        let t = Target::from_pc(&pc).unwrap();
        assert_eq!(t.os, "linux");
        assert_eq!(t.capture_size, Some([1920, 1080]));
        assert_eq!(t.max_fps, 30);
        let old = Target::from_pc(&Pc { host: None, ..pc }).unwrap();
        assert_eq!((old.capture_size, old.max_fps), (None, 0));
    }

    #[test]
    fn update_reservation_blocks_connect_in_every_idle_state() {
        for step in [Step::Idle, Step::Ended { error: None }] {
            let mut p = Progress {
                step,
                ..Default::default()
            };
            assert!(!p.active());
            p.updating = true;
            assert!(p.active());
        }
        for step in [
            Step::Waking,
            Step::Waiting,
            Step::Pairing { pin: "1234".into() },
            Step::Connecting,
            Step::Streaming,
        ] {
            assert!(Progress {
                step,
                ..Default::default()
            }
            .active());
        }
    }

    /// Against Latch Host running on this machine: the packet goes out on
    /// the LAN and the host reports it. `cargo test -p latch-client wake_test_real -- --ignored`
    #[test]
    #[ignore = "needs Latch Host running on this machine"]
    fn wake_test_real() {
        let ip: Ipv4Addr = "127.0.0.1".parse().unwrap();
        let host = host_status(ip, Duration::from_secs(2)).expect("host status");
        let pc = Pc {
            name: host.name.clone(),
            ip: Some(ip),
            online: true,
            known: Some(KnownPc {
                mac: host.mac.clone(),
                lan_ip: host.lan_ip.clone(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(wake_test(&pc).unwrap());
    }

    #[test]
    fn a_pc_is_streamable_when_reachable_or_wakeable() {
        let mut pc = Pc {
            ip: Some("203.0.113.10".parse().unwrap()),
            ..Default::default()
        };
        assert!(!pc.can_stream());
        pc.sunshine = true;
        assert!(pc.can_stream());
        pc.sunshine = false;
        pc.known = Some(KnownPc {
            mac: Some("02:00:00:00:00:01".into()),
            ..Default::default()
        });
        assert!(pc.can_wake() && pc.can_stream());
        pc.ip = None;
        assert!(!pc.can_stream());
    }

    #[test]
    fn target_parses_what_it_learned() {
        let pc = Pc {
            name: "Gaming-PC".into(),
            ip: Some("203.0.113.10".parse().unwrap()),
            online: true,
            sunshine: true,
            known: Some(KnownPc {
                name: "Gaming-PC".into(),
                mac: Some("02:00:00:00:00:01".into()),
                lan_ip: Some("192.168.1.10".into()),
                public_ip: Some("203.0.113.5".into()),
                server_cert: Some("3082".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let t = Target::from_pc(&pc).unwrap();
        assert!(t.online);
        assert_eq!(t.mac.unwrap().to_string(), "02:00:00:00:00:01");
        assert_eq!(t.lan_ip, Some("192.168.1.10".parse().unwrap()));
        assert_eq!(t.public_ip, Some("203.0.113.5".parse().unwrap()));
        assert_eq!(t.server_cert, Some(vec![0x30, 0x82]));
    }

    /// `cargo test -p latch-client scan_real -- --ignored --nocapture`
    /// Connects to `LATCH_TEST_PC` (its Tailscale name) through the
    /// window's own path, once for each of `LATCH_TEST_RESOLUTIONS` in turn
    /// ("540p,720p,1080p,native"), with this Mac's identity and pairing, and
    /// leaves the PC's Desktop running as the window does. At each size it
    /// puts the PC's pointer at the stream's centre and a quarter in, then
    /// moves it 100 stream pixels right, running `LATCH_TEST_PROBE` (a shell
    /// command, say one that prints the pointer and the display size over
    /// ssh) after each step.
    #[test]
    #[ignore = "needs Tailscale and a paired PC"]
    fn connect_real() {
        use crate::config::{Quality, Resolution};
        let _ = tracing_subscriber::fmt().with_target(false).try_init();
        let name = std::env::var("LATCH_TEST_PC").expect("LATCH_TEST_PC");
        let probe = || {
            let Ok(cmd) = std::env::var("LATCH_TEST_PROBE") else {
                return;
            };
            std::thread::sleep(Duration::from_millis(400));
            let out = std::process::Command::new("sh")
                .args(["-c", &cmd])
                .output()
                .expect("probe");
            eprint!("    probe: {}", String::from_utf8_lossy(&out.stdout));
        };
        let list = std::env::var("LATCH_TEST_RESOLUTIONS").unwrap_or_else(|_| "720p".into());
        for label in list.split(',') {
            let resolution = Resolution::ALL
                .into_iter()
                .find(|r| r.label() == label || (label == "native" && *r == Resolution::Native))
                .unwrap_or_else(|| panic!("no resolution {label}"));
            // Afresh each time, as the window does, so a pairing made by the
            // first connect is used by the next.
            let cfg = ClientConfig::load();
            let discovery = scan(&cfg);
            let pc = discovery
                .pcs
                .iter()
                .find(|p| p.name == name)
                .unwrap_or_else(|| panic!("{name} is not on the tailnet"));
            let c = Connect {
                target: Target::from_pc(pc).expect("target"),
                settings: StreamSettings {
                    quality: Quality::Custom,
                    resolution,
                    fps: 60,
                    bitrate_kbps: 10_000,
                    ..cfg.stream.clone()
                },
                restart_capture: false,
                native: (3024, 1964),
                progress: Arc::default(),
                live: Arc::default(),
                ctx: egui::Context::default(),
            };
            let generation = NEXT_CONNECT.fetch_add(1, Ordering::Relaxed);
            c.progress.lock().generation = generation;
            run(&c, generation).expect("connect");
            let live = c.live.lock().take().expect("a stream");
            let start = Instant::now();
            loop {
                match live.events.recv_timeout(Duration::from_secs(20)) {
                    Ok(Event::Connected) => break,
                    Ok(Event::Failed { .. } | Event::Terminated { .. }) | Err(_) => {
                        panic!("no stream at {label}")
                    }
                    Ok(_) => {}
                }
            }
            let (w, h, fps) = live.requested;
            eprintln!(
                "{label}: asked {w}x{h} at {fps} fps, desktop {:?}, connected in {:?}",
                live.desktop,
                start.elapsed()
            );
            std::thread::sleep(Duration::from_secs(2));
            let (iw, ih) = (w as i16, h as i16);
            eprintln!("  pointer to the centre, {}x{}", w / 2, h / 2);
            live.input.mouse_position(iw / 2, ih / 2, iw, ih);
            probe();
            eprintln!("  pointer to a quarter in, {}x{}", w / 4, h / 4);
            live.input.mouse_position(iw / 4, ih / 4, iw, ih);
            probe();
            eprintln!("  100 stream pixels right, in steps of 10");
            for _ in 0..10 {
                live.input.mouse_move(10, 0);
                std::thread::sleep(Duration::from_millis(16));
            }
            probe();
            eprintln!("  100 stream pixels right at once");
            live.input.mouse_move(100, 0);
            probe();
            // Letters, digits and spaces as key presses, then Return.
            if let Ok(line) = std::env::var("LATCH_TEST_TYPE") {
                eprintln!("  typing {line:?}");
                for c in line.chars().map(|c| c.to_ascii_uppercase()) {
                    assert!(c.is_ascii_alphanumeric() || c == ' ', "can't type {c:?}");
                    for down in [true, false] {
                        live.input.key(c as i16, down, 0);
                        std::thread::sleep(Duration::from_millis(15));
                    }
                }
                for down in [true, false] {
                    live.input.key(crate::input::VK_RETURN, down, 0);
                }
                probe();
            }
            let seconds: u64 =
                std::env::var("LATCH_TEST_SECONDS").map_or(0, |s| s.parse().expect("seconds"));
            for second in 1..=seconds {
                std::thread::sleep(Duration::from_secs(1));
                let s = live.session.stats();
                eprintln!(
                    "  {second:>2}s {:.1} fps, {:.1} Mbps, loss {:.1}%, host {:.1} ms",
                    s.fps, s.mbps, s.loss_pct, s.host_ms
                );
            }
            live.session.stop();
            let t = Instant::now();
            while !live.session.finished() && t.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(live);
        }
    }

    #[test]
    #[ignore = "needs Tailscale"]
    fn scan_real_tailnet() {
        let d = scan(&ClientConfig::load());
        eprintln!("error={:?} login={} pcs={}", d.error, d.login, d.pcs.len());
        for pc in &d.pcs {
            eprintln!(
                "  {} ip={:?} online={} host={} sunshine={} wake={} known={:?}",
                pc.name,
                pc.ip,
                pc.online,
                pc.host.is_some(),
                pc.sunshine,
                pc.can_wake(),
                pc.known
            );
        }
        assert!(d.error.is_some() || !d.login.is_empty());
    }

    #[test]
    fn hevc_needs_the_setting_the_pc_and_this_decoder() {
        let hevc_pc = 0x0101;
        assert!(!wants_hevc(Codec::H264, hevc_pc));
        assert!(!wants_hevc(Codec::Auto, 0x0001), "an H.264-only PC");
        assert_eq!(
            wants_hevc(Codec::Auto, hevc_pc),
            cfg!(target_os = "macos"),
            "only VideoToolbox decodes HEVC here"
        );
    }

    #[test]
    fn a_cancelled_retry_stops_at_once() {
        let progress = Mutex::new(Progress {
            generation: 3,
            ..Default::default()
        });
        let mut tries = 0;
        let r: Result<()> = retry(8, &progress, 3, || {
            tries += 1;
            progress.lock().cancel = true;
            bail!("the PC is not answering")
        });
        assert_eq!(tries, 1);
        assert_eq!(r.unwrap_err().to_string(), "the PC is not answering");
        progress.lock().cancel = false;
        let mut tries = 0;
        let r = retry(3, &progress, 3, || {
            tries += 1;
            if tries < 2 {
                bail!("not yet")
            }
            Ok(tries)
        });
        assert_eq!(r.unwrap(), 2);
    }

    #[test]
    fn only_a_scan_that_changes_something_redraws() {
        let first = Discovery {
            refreshed: Some(Instant::now()),
            pcs: vec![Pc {
                name: "Gaming-PC".into(),
                online: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(
            listing_changed(&Discovery::default(), &first),
            "the first scan"
        );
        let again = Discovery {
            refreshed: Some(Instant::now() + Duration::from_secs(3)),
            ..first.clone()
        };
        assert!(!listing_changed(&first, &again), "only the time moved");
        let mut asleep = again.clone();
        asleep.pcs[0].online = false;
        assert!(listing_changed(&first, &asleep));
        let mut relay = again;
        relay.peer_relay_servers = PeerRelayServers::Known(vec!["100.64.0.40".into()]);
        assert!(listing_changed(&first, &relay));
    }

    #[test]
    fn progress_tracks_step_changes() {
        let mut p = Progress::default();
        assert!(!p.active());
        p.set(Step::Waking, "x");
        assert!(p.active());
        let since = p.since;
        std::thread::sleep(Duration::from_millis(5));
        p.set(Step::Waking, "y");
        assert_eq!(p.since, since);
        p.set(Step::Ended { error: None }, "");
        assert!(!p.active());
    }
}
