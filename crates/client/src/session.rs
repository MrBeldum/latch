//! Finding PCs on the tailnet, and the path from "asleep in another room" to
//! a live stream: wake, wait, check the host, connect.

use crate::clipboard;
use crate::config::{ClientConfig, Codec, KnownPc, StreamSettings};
use crate::path::{self, Path};
use anyhow::{anyhow, bail, Context, Result};
use latch_core::api::{Ack, NatReport, PowerAction, PowerRequest, Status};
use latch_core::{http, tailscale, wake, CONTROL_PORT};
use latch_stream::session::Server;
use latch_stream::{Event, FrameSlot, Input, Session, Settings};
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet};
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
    /// The Latch control service answered during discovery.
    pub online: bool,
    /// The Latch control service answered.
    pub host: Option<Status>,
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
        self.ip.is_some() && (self.host.is_some() || self.can_wake())
    }
    pub fn power_allowed(&self) -> bool {
        self.host.as_ref().is_some_and(|h| h.power_allowed)
    }
    /// Sleep is offered only for a Windows PC, which a wake packet brings
    /// back. A Linux machine (a cloud VPS above all) or a Mac would stay
    /// asleep with nothing here able to wake it.
    pub fn can_sleep(&self) -> bool {
        self.power_allowed() && sleep_offered(&self.os)
    }
}

/// Whether Latch offers to put a machine with Tailscale's OS string `os`
/// to sleep.
pub fn sleep_offered(os: &str) -> bool {
    os.eq_ignore_ascii_case("windows")
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
    /// Every node Tailscale lists on this tailnet, asleep or not, relays and
    /// phones included; `None` when Tailscale could not say.
    pub listed: Option<BTreeSet<String>>,
    /// The tailnet this machine is on, when Tailscale names it.
    pub tailnet: Option<String>,
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

/// A saved machine that has been off the tailnet this long is forgotten.
const FORGET_AFTER: u64 = 30 * 86_400;

/// Save what a scan taught about each PC: address, wake details, when it
/// was last seen. Forget the ones that have left the tailnet.
fn learn(scan: &Discovery) {
    if let Err(e) = ClientConfig::update(|learned| {
        learn_into(learned, scan, latch_core::dates::now_unix());
    }) {
        tracing::warn!("could not save what was learned: {e:#}");
    }
}

fn learn_into(learned: &mut ClientConfig, scan: &Discovery, now: u64) {
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
        if scan.tailnet.is_some() {
            entry.tailnet = scan.tailnet.clone();
        }
        if pc.online {
            // Coarse on purpose: the file is rewritten only when this moves.
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
    if let Some(listed) = &scan.listed {
        for id in retired(&learned.pcs, listed, scan.tailnet.as_deref(), now) {
            if let Some(k) = learned.pcs.remove(&id) {
                tracing::info!("forgot {} ({id}): it has left the tailnet", k.name);
            }
        }
    }
}

/// Saved machines that have left the tailnet this machine is on: Tailscale
/// no longer lists them (it lists every machine there, asleep or not) and
/// nothing has seen them for [`FORGET_AFTER`]. A machine saved on another
/// tailnet is kept for when this one goes back; one saved before tailnets
/// were recorded counts as this tailnet's only when another saved machine
/// is listed here. An empty listing proves nothing and forgets nothing.
fn retired(
    pcs: &BTreeMap<String, KnownPc>,
    listed: &BTreeSet<String>,
    tailnet: Option<&str>,
    now: u64,
) -> Vec<String> {
    if listed.is_empty() {
        return Vec::new();
    }
    let here = pcs.keys().any(|id| listed.contains(id));
    pcs.iter()
        .filter(|(id, k)| {
            let ours = match (k.tailnet.as_deref(), tailnet) {
                (Some(theirs), Some(current)) => theirs == current,
                (Some(_), None) => false,
                (None, _) => here,
            };
            !listed.contains(*id)
                && ours
                && k.last_seen_unix
                    .is_none_or(|t| now.saturating_sub(t) >= FORGET_AFTER)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

fn scan_public(pc: &Pc) -> Option<Ipv4Addr> {
    saved_public(pc.known.as_ref()?.public_ip.as_ref())
}

/// A saved public address, unless it is not one: older versions saved
/// whatever address Tailscale last reached the machine on, which can be its
/// LAN address, and a wake packet sent there goes nowhere.
fn saved_public(s: Option<&String>) -> Option<Ipv4Addr> {
    s?.parse()
        .ok()
        .filter(|ip| latch_core::tailscale::is_public_v4(*ip))
}

/// Probe every peer at once: each can take two timeouts when nothing
/// answers, and a tailnet of a dozen machines would otherwise make one
/// refresh of the list take half a minute.
fn probe_all(cfg: &ClientConfig, peers: &[&tailscale::Node]) -> Vec<Pc> {
    std::thread::scope(|s| {
        let probes: Vec<_> = peers
            .iter()
            .map(|&n| s.spawn(move || probe_peer(cfg, n)))
            .collect();
        probes.into_iter().filter_map(|p| p.join().ok()).collect()
    })
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
    let mut pcs = probe_all(cfg, &peers);
    // A tag:relay node that also runs Latch (a VPS desktop on the same
    // box as the packet relay) belongs in the machine list too.
    pcs.extend(
        probe_all(cfg, &st.relay_peers())
            .into_iter()
            .filter(|pc| pc.host.is_some()),
    );
    pcs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut listed: BTreeSet<String> = st.peer.values().map(|n| n.id.clone()).collect();
    if std::env::var_os("LATCH_DEV_LOCAL").is_some() {
        listed.insert("local".into());
    }
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
        listed: Some(listed),
        tailnet: st
            .current_tailnet
            .as_ref()
            .map(|t| t.name.clone())
            .filter(|n| !n.is_empty()),
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
    let (host, rtt_ms) = match (n.online, ip) {
        (true, Some(ip)) => {
            let host = host_status(ip, Duration::from_millis(900));
            let (_, rtt) = timed_port_open(ip, CONTROL_PORT, Duration::from_millis(900));
            (host, rtt)
        }
        _ => (None, None),
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
        online: host.is_some(),
        host,
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
        if let (true, Some(ms)) = timed_port_open(ip, CONTROL_PORT, Duration::from_millis(1500)) {
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
    Launching,
    /// The stream is being negotiated; the text is the current stage.
    Connecting,
    Streaming,
    Ended {
        error: Option<String>,
    },
}

impl Step {
    /// For the log: the current step.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Waking => "waking",
            Self::Waiting => "waiting",
            Self::Launching => "launching",
            Self::Connecting => "connecting",
            Self::Streaming => "streaming",
            Self::Ended { .. } => "ended",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub pc: String,
    /// The machine's Tailscale node id; names are not unique.
    pub node_id: String,
    /// Its Tailscale address, which power actions are sent to.
    pub ip: Option<Ipv4Addr>,
    pub step: Step,
    pub detail: String,
    pub since: Instant,
    pub cancel: bool,
    /// Blocks new connections while the updater swaps and relaunches this app.
    pub updating: bool,
    /// Distinguishes this attempt from an older worker that is still running.
    pub generation: u64,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            pc: String::new(),
            node_id: String::new(),
            ip: None,
            step: Step::Idle,
            detail: String::new(),
            since: Instant::now(),
            cancel: false,
            updating: false,
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
        self.input.release_all();
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
            online: pc.host.is_some(),
            mac: k
                .and_then(|k| k.mac.as_deref())
                .and_then(wake::MacAddr::parse),
            lan_ip: parse(k.and_then(|k| k.lan_ip.as_ref())),
            public_ip: saved_public(k.and_then(|k| k.public_ip.as_ref())),
            path: pc.path.clone(),
            os: pc.os.clone(),
            capture_size: pc.host.as_ref().and_then(|h| h.streamer.capture_size),
            max_fps: pc.host.as_ref().map_or(0, |h| h.streamer.max_fps),
        })
    }
}

/// What to ask a PC for, from what was wanted: its own capture size when
/// that is fixed and the wanted size is larger in either direction
/// (scaling the same desktop up blurs it and costs the encoder more; a
/// smaller size is scaled down and is cheaper to encode), and no more
/// frames a second than it keeps up with. The host's
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

/// What a stream with `settings` asks the PC for on a display of `native`
/// pixels, after Auto has had its say and the PC's limits theirs: width,
/// height and frame rate. A connect asks for exactly this.
pub fn request_for(
    settings: &StreamSettings,
    native: (u32, u32),
    capture_size: Option<[u32; 2]>,
    max_fps: u32,
) -> (u32, u32, u32) {
    let s = path::effective(settings);
    fit_to_host(s.resolution.pixels(native), s.fps, capture_size, max_fps)
}

pub struct Connect {
    pub target: Target,
    pub settings: StreamSettings,
    pub native: (u32, u32),
    pub progress: Arc<Mutex<Progress>>,
    pub live: Arc<Mutex<Option<Live>>>,
    pub ctx: egui::Context,
}

/// Wake, wait, check the host, connect: on its own thread, reporting into
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
            node_id: c.target.node_id.clone(),
            ip: Some(c.target.ip),
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
            Err(e) => {
                // The step it got to, and every cause: the banner alone
                // leaves nothing to look at afterwards.
                tracing::warn!(
                    "couldn't connect to {} ({}): {e:#}",
                    c.target.name,
                    if p.detail.is_empty() {
                        p.step.name()
                    } else {
                        p.detail.as_str()
                    }
                );
                p.set(
                    Step::Ended {
                        error: Some(format!("{e:#}")),
                    },
                    "",
                )
            }
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
    if !t.online && !port_open(t.ip, CONTROL_PORT, Duration::from_millis(1200)) {
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
            if port_open(t.ip, CONTROL_PORT, Duration::from_millis(1500)) {
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

    // Wait for the control service, then require the current stream protocol.
    let start = Instant::now();
    let mut host = loop {
        if stale(&c.progress, generation) {
            bail!("cancelled");
        }
        if let Some(host) = host_status(t.ip, Duration::from_millis(1500)) {
            break host;
        }
        report(
            Step::Waiting,
            format!("Waiting for {}… {}s", t.name, start.elapsed().as_secs()),
        );
        if start.elapsed() > Duration::from_secs(60) {
            bail!(
                "{} is on, but Latch isn't answering. Open Latch there and set up sharing on its Sharing tab.",
                t.name
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    let os = if host.os.is_empty() { &t.os } else { &host.os };
    if host.streamer.protocol < 1 {
        if !can_upgrade(os, &host.version) {
            bail!("{}", incompatible_host(&t.name, os));
        }
        report(Step::Waiting, format!("Updating Latch on {}…", t.name));
        crate::update::upgrade_for_stream(t.ip, &host.version)
            .with_context(|| format!("Updating Latch on {}", t.name))?;
        let start = Instant::now();
        loop {
            if stale(&c.progress, generation) {
                bail!("cancelled");
            }
            if let Some(updated) = host_status(t.ip, Duration::from_secs(2)) {
                if updated.streamer.protocol >= 1 {
                    host = updated;
                    break;
                }
            }
            if start.elapsed() >= Duration::from_secs(120) {
                bail!("Latch on {} hasn't returned with the new streaming version. Open Latch there and check its update.", t.name);
            }
            report(
                Step::Waiting,
                format!("Waiting for {} to finish updating…", t.name),
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    if stale(&c.progress, generation) {
        bail!("cancelled");
    }

    report(Step::Launching, "Measuring the path…".into());
    let path = measure_path(&t.node_id, t.ip, &t.path);
    let mut settings = path::effective(&c.settings);
    let wanted = settings.resolution.pixels(c.native);
    let (w, h, fps) = request_for(
        &c.settings,
        c.native,
        host.streamer.capture_size,
        host.streamer.max_fps,
    );
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
    if stale(&c.progress, generation) {
        bail!("cancelled");
    }
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = c.ctx.clone();
    let session = Session::start(
        Server {
            address: IpAddr::V4(t.ip),
            port: CONTROL_PORT,
        },
        Settings {
            width: w,
            height: h,
            fps,
            bitrate_kbps: settings.bitrate_kbps,
            hevc: settings.codec != Codec::H264,
            audio: true,
            name: latch_core::config::machine_name(),
        },
        frames.clone(),
        tx,
        move || ctx.request_repaint(),
    );
    if stale(&c.progress, generation) {
        drop(session);
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
        requested: (w, h, fps),
        desktop: fixed_desktop(host.streamer.capture_size).filter(|&d| d != (w, h)),
        settings,
        path,
        clipboard: clipboard::Sync::spawn(t.ip, c.ctx.clone()),
        shortcuts: clipboard::Shortcuts::for_os(if host.os.is_empty() { &t.os } else { &host.os }),
    });
    report(Step::Connecting, "Connecting…".into());
    Ok(())
}

/// Whether this host accepts the existing Windows update push.
fn can_upgrade(os: &str, version: &str) -> bool {
    latch_core::update::takes_pushed_host(os)
        && semver::Version::parse(version)
            .is_ok_and(|v| latch_core::update::host_can_receive_update(&v))
}

fn incompatible_host(name: &str, os: &str) -> String {
    let action = if os.eq_ignore_ascii_case("macos") {
        "open Latch on that Mac and turn on automatic updates in Settings; its app updates itself."
    } else if os.eq_ignore_ascii_case("linux") {
        "install the latest latch-host-linux release tarball, following deploy/native/README.md."
    } else {
        "install the latest Latch release from https://github.com/MrBeldum/latch/releases/latest."
    };
    format!(
        "{name} runs Latch 4.x, which streams differently. Update Latch there to connect: {action}"
    )
}

/// Ask the host to change the machine's power state.
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
    fn old_hosts_get_a_push_or_specific_update_instructions() {
        assert!(can_upgrade("windows", "4.1.1"));
        assert!(!can_upgrade("windows", "3.0.0"));
        assert!(!can_upgrade("windows", "unknown"));
        for os in ["macOS", "linux", ""] {
            assert!(!can_upgrade(os, "4.1.1"));
            let message = incompatible_host("Studio", os);
            assert!(message.starts_with(
                "Studio runs Latch 4.x, which streams differently. Update Latch there to connect: "
            ));
            assert!(message.ends_with('.'));
        }
        assert!(incompatible_host("Studio", "macOS").contains("its app updates itself"));
        assert!(incompatible_host("Studio", "linux").contains("deploy/native/README.md"));
        let old: Status = serde_json::from_str(r#"{"app":"latch","streamer":{}}"#).unwrap();
        assert_eq!(old.streamer.protocol, 0);
    }

    #[test]
    fn a_saved_lan_address_is_not_taken_for_the_public_one() {
        let saved = |s: &str| saved_public(Some(&s.to_string()));
        assert_eq!(
            saved("116.125.113.36"),
            Some("116.125.113.36".parse().unwrap())
        );
        assert_eq!(saved("192.168.55.57"), None);
        assert_eq!(saved("10.0.2.15"), None);
        assert_eq!(saved("100.106.23.40"), None, "a Tailscale address");
        assert_eq!(saved("not an address"), None);
        assert_eq!(saved_public(None), None);
    }

    #[test]
    fn the_request_depends_on_the_display_only_where_the_setting_does() {
        use crate::config::{Quality, Resolution};
        let (laptop, monitor) = ((3024, 1964), (2560, 1440));
        let custom = |resolution| StreamSettings {
            quality: Quality::Custom,
            resolution,
            fps: 60,
            ..Default::default()
        };
        // A fixed size asks for the same whichever display the window is on.
        let p1080 = custom(Resolution::P1080);
        assert_eq!(
            request_for(&p1080, laptop, None, 0),
            request_for(&p1080, monitor, None, 0)
        );
        // Match screen follows the display...
        let native = custom(Resolution::Native);
        assert_ne!(
            request_for(&native, laptop, None, 0),
            request_for(&native, monitor, None, 0)
        );
        // ...unless the PC's fixed desktop clamps both to its own size.
        let vps = Some([1920, 1080]);
        assert_eq!(
            request_for(&native, laptop, vps, 30),
            request_for(&native, monitor, vps, 30)
        );
        // Auto is Match screen.
        let auto = StreamSettings::default();
        assert_eq!(auto.quality, Quality::Auto);
        assert_eq!(request_for(&auto, laptop, None, 0).0, 3024);
        assert_eq!(request_for(&auto, monitor, None, 0).0, 2560);
    }

    fn known(tailnet: Option<&str>, seen_days_ago: Option<u64>) -> KnownPc {
        KnownPc {
            name: "pc".into(),
            tailnet: tailnet.map(str::to_string),
            last_seen_unix: seen_days_ago.map(|d| NOW - d * 86_400),
            ..Default::default()
        }
    }

    const NOW: u64 = 1_792_000_000;

    #[test]
    fn a_machine_that_left_the_tailnet_a_month_ago_is_forgotten() {
        let here = Some("daniel@example.com");
        let mut pcs = BTreeMap::new();
        pcs.insert("hermes".to_string(), known(here, Some(90)));
        pcs.insert("artemis".to_string(), known(None, Some(31)));
        pcs.insert("recent".to_string(), known(here, Some(2)));
        pcs.insert("never-seen".to_string(), known(here, None));
        pcs.insert("work".to_string(), known(Some("work.example"), Some(400)));
        // Hermes is listed (asleep for months, still on the tailnet); a
        // relay listed by Tailscale but not among the PCs is listed too.
        let listed: BTreeSet<String> = ["hermes", "relay"].map(String::from).into();
        let gone = retired(&pcs, &listed, here, NOW);
        assert_eq!(gone, ["artemis", "never-seen"], "{gone:?}");
        // Nothing listed, or nothing to say it is this tailnet: nothing goes.
        assert!(retired(&pcs, &BTreeSet::new(), here, NOW).is_empty());
        let elsewhere: BTreeSet<String> = ["someone-else"].map(String::from).into();
        assert_eq!(
            retired(&pcs, &elsewhere, Some("other.example"), NOW),
            Vec::<String>::new(),
            "on another tailnet, this one's machines are kept"
        );
        // A Tailscale that names no tailnet forgets only unmarked entries,
        // and only when a saved machine is listed.
        assert_eq!(retired(&pcs, &listed, None, NOW), ["artemis"]);
    }

    #[test]
    fn learning_marks_the_tailnet_and_a_failed_scan_forgets_nothing() {
        let mut cfg = ClientConfig::default();
        cfg.pcs.insert("artemis".into(), known(None, Some(40)));
        cfg.pcs.insert("hermes".into(), known(None, Some(1)));
        let hermes = Pc {
            node_id: "hermes".into(),
            name: "Hermes".into(),
            ..Default::default()
        };
        // Tailscale down: the list is what was saved, and nothing changes.
        let down = Discovery {
            error: Some("Tailscale is stopped".into()),
            pcs: remembered(&cfg),
            ..Default::default()
        };
        let before = cfg.clone();
        learn_into(&mut cfg, &down, NOW);
        assert_eq!(cfg, before);
        // Tailscale up: Hermes is listed and marked; artemis is not.
        let up = Discovery {
            pcs: vec![hermes],
            listed: Some(["hermes".to_string()].into()),
            tailnet: Some("daniel@example.com".into()),
            ..Default::default()
        };
        learn_into(&mut cfg, &up, NOW);
        assert_eq!(cfg.pcs.keys().collect::<Vec<_>>(), ["hermes"]);
        assert_eq!(
            cfg.pcs["hermes"].tailnet.as_deref(),
            Some("daniel@example.com")
        );
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
        pc.host = Some(Status::default());
        assert!(pc.can_stream());
        pc.host = None;
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
            host: Some(Status::default()),
            known: Some(KnownPc {
                name: "Gaming-PC".into(),
                mac: Some("02:00:00:00:00:01".into()),
                lan_ip: Some("192.168.1.10".into()),
                public_ip: Some("203.0.113.5".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let t = Target::from_pc(&pc).unwrap();
        assert!(t.online);
        assert_eq!(t.mac.unwrap().to_string(), "02:00:00:00:00:01");
        assert_eq!(t.lan_ip, Some("192.168.1.10".parse().unwrap()));
        assert_eq!(t.public_ip, Some("203.0.113.5".parse().unwrap()));
    }

    /// `LATCH_TEST_PC=name-or-ip cargo test -p latch-client connect_real -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a live Latch 5 host and completed protocol/codec implementations"]
    fn connect_real() {
        use crate::config::{Quality, Resolution};
        let _ = tracing_subscriber::fmt().with_target(false).try_init();
        let name = std::env::var("LATCH_TEST_PC").expect("LATCH_TEST_PC");
        let cfg = ClientConfig::load();
        let discovery = scan(&cfg);
        let pc = discovery
            .pcs
            .iter()
            .find(|p| p.name == name || p.ip.is_some_and(|ip| ip.to_string() == name))
            .cloned()
            .unwrap_or_else(|| Pc {
                name: name.clone(),
                node_id: name.clone(),
                ip: Some(
                    name.parse()
                        .expect("LATCH_TEST_PC must name a host or be an IPv4 address"),
                ),
                ..Default::default()
            });
        let seconds = std::env::var("LATCH_TEST_SECONDS")
            .map_or(5, |s| s.parse::<u64>().expect("seconds"))
            .max(1);
        let list = std::env::var("LATCH_TEST_RESOLUTIONS").unwrap_or_else(|_| "720p".into());
        for label in list.split(',').map(str::trim) {
            let resolution = Resolution::ALL
                .into_iter()
                .find(|r| r.label() == label || (label == "native" && *r == Resolution::Native))
                .unwrap_or_else(|| panic!("no resolution {label}"));
            let c = Connect {
                target: Target::from_pc(&pc).expect("target"),
                settings: StreamSettings {
                    quality: Quality::Custom,
                    resolution,
                    fps: 60,
                    bitrate_kbps: 10_000,
                    ..cfg.stream.clone()
                },
                native: (3024, 1964),
                progress: Arc::default(),
                live: Arc::default(),
                ctx: egui::Context::default(),
            };
            let generation = NEXT_CONNECT.fetch_add(1, Ordering::Relaxed);
            c.progress.lock().generation = generation;
            run(&c, generation).expect("connect");
            let live = c.live.lock().take().expect("a stream");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                assert!(Instant::now() < deadline, "no stream at {label}");
                match live.events.recv_timeout(Duration::from_secs(1)) {
                    Ok(Event::Connected) => break,
                    Ok(Event::Failed { message, .. } | Event::Terminated { message, .. }) => {
                        panic!("{label}: {message}")
                    }
                    Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(e) => panic!("{label}: {e}"),
                }
            }
            for second in 1..=seconds {
                std::thread::sleep(Duration::from_secs(1));
                let s = live.session.stats();
                eprintln!("{label} {second}s: {}x{} {} · {:.1} fps, {:.1} Mbps, loss {:.1}%, host {:.1} ms, decode {:.1} ms · {} → {}", s.width, s.height, s.codec, s.fps, s.mbps, s.loss_pct, s.host_ms, s.decode_ms, s.encoder, s.decoder);
                assert!(!live.session.finished(), "{label}: stream ended early");
            }
            assert!(live.frames.seq() > 0, "{label}: no frames arrived");
            live.input.release_all();
            live.session.stop();
            let start = Instant::now();
            while !live.session.finished() && start.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(live.session.finished(), "{label}: stream did not stop");
        }
    }

    #[test]
    #[ignore = "needs Tailscale"]
    fn scan_real_tailnet() {
        let d = scan(&ClientConfig::load());
        eprintln!("error={:?} login={} pcs={}", d.error, d.login, d.pcs.len());
        for pc in &d.pcs {
            eprintln!(
                "  {} ip={:?} online={} host={} wake={} known={:?}",
                pc.name,
                pc.ip,
                pc.online,
                pc.host.is_some(),
                pc.can_wake(),
                pc.known
            );
        }
        assert!(d.error.is_some() || !d.login.is_empty());
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
